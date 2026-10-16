//! Bind an image tool to the account selected for its exact Codex turn.
use crate::app::ServerState;
use crate::services::request_preparation::{RequestOperation, prepare_request};
use emp_core::{Dialect, ResolvedRoute, resolved_route_from_parts};
use emp_router::native_http::NativeHttpError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const CAPACITY: usize = 4096;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

struct Binding {
    turn: String,
    model: String,
    provider: String,
    updated: Instant,
}

#[derive(Default)]
pub(crate) struct ImageRoutes(Mutex<VecDeque<Binding>>);

impl ImageRoutes {
    pub(crate) fn remember(
        &self,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
    ) {
        let Some(turn) = body
            .as_object()
            .and_then(|body| emp_history::request_history_anchor(body, incoming).ok())
            .and_then(|anchor| anchor.turn_id)
        else {
            return;
        };
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|entry| entry.updated.elapsed() < RETENTION && entry.turn != turn);
        if entries.len() == CAPACITY {
            entries.pop_front();
        }
        entries.push_back(Binding {
            turn,
            model: route.requested_model.clone(),
            provider: route.provider_id.clone(),
            updated: Instant::now(),
        });
    }

    pub(super) fn resolve(
        &self,
        state: &ServerState,
        body: &Value,
        incoming: &BTreeMap<String, String>,
    ) -> Result<ResolvedRoute, NativeHttpError> {
        let model = body["model"]
            .as_str()
            .filter(|model| !model.trim().is_empty())
            .ok_or_else(|| NativeHttpError::router(400, "image request.model is required"))?;
        let turn = incoming
            .get("x-codex-image-turn-id")
            .filter(|turn| !turn.is_empty())
            .ok_or_else(|| {
                NativeHttpError::router(400, "image request requires x-codex-image-turn-id")
            })?;
        let (selected, provider) = self.0.lock().unwrap_or_else(PoisonError::into_inner).iter()
            .find(|entry| &entry.turn == turn && entry.updated.elapsed() < RETENTION)
            .map(|entry| (entry.model.clone(), entry.provider.clone()))
            .ok_or_else(|| NativeHttpError::router(409,
                "The image tool's account could not be matched to its Codex turn. Retry in a new turn."))?;
        // Resolve current settings again: disabling or deleting an account must
        // also stop its image tools, without retaining a credential snapshot.
        let prepared = prepare_request(state, json!({"model":selected}), RequestOperation::Images)
            .map_err(|_| {
                NativeHttpError::router(
                    409,
                    "The image tool's selected account is no longer available",
                )
            })?;
        let route = prepared.route;
        if route.provider_id != provider || route.dialect != Dialect::CodexNative {
            return Err(NativeHttpError::router(
                422,
                "The selected service does not support Codex image tools",
            ));
        }
        let prefix = route
            .provider
            .value()
            .get("account")
            .and_then(|account| account["prefix"].as_str());
        let requested =
            prefix.map_or_else(|| model.to_owned(), |prefix| format!("{prefix}/{model}"));
        resolved_route_from_parts(
            &requested,
            route.provider.value().clone(),
            json!({"id":requested,"upstream_id":model})
                .as_object()
                .unwrap()
                .clone(),
            route.source,
        )
        .map_err(|_| NativeHttpError::router(500, "image route could not be prepared"))
    }
}
