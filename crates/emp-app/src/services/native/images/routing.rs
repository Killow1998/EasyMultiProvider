//! Freeze image account choice per turn; never replay an ambiguous image outcome.
use crate::app::ServerState;
use crate::services::request_preparation::{RequestOperation, prepare_request};
use emp_core::{Dialect, ResolvedRoute, RouteSource, resolved_route_from_parts};
use emp_router::native_http::NativeHttpError;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

mod affinity;
mod selection;
const CAPACITY: usize = 4096;
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
enum Target {
    Follow { model: String, provider: String },
    Account(String),
}

struct Binding {
    turn: String,
    model: String,
    provider: String,
    target: Option<Target>,
    updated: Instant,
}

#[derive(Default)]
struct Routes {
    turns: VecDeque<Binding>,
    limited: BTreeMap<String, Instant>,
    assets: affinity::Assets,
}

#[derive(Default)]
pub(crate) struct ImageRoutes(Mutex<Routes>);

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
        let mut routes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        routes
            .turns
            .retain(|entry| entry.updated.elapsed() < RETENTION);
        if let Some(entry) = routes.turns.iter_mut().find(|entry| entry.turn == turn) {
            entry.model.clone_from(&route.requested_model);
            entry.provider.clone_from(&route.provider_id);
            entry.updated = Instant::now();
            return;
        }
        if routes.turns.len() == CAPACITY {
            routes.turns.pop_front();
        }
        routes.turns.push_back(Binding {
            turn,
            model: route.requested_model.clone(),
            provider: route.provider_id.clone(),
            target: None,
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
        let config = state
            .backend
            .configuration
            .read()
            .map_err(|_| {
                NativeHttpError::router(503, "image routing configuration is unavailable")
            })?
            .clone();
        let (target, asset_account) = {
            let mut routes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            routes.limited.retain(|_, until| *until > Instant::now());
            let index = routes.turns.iter().position(|entry| &entry.turn == turn && entry.updated.elapsed() < RETENTION)
                .ok_or_else(|| NativeHttpError::router(409,
                    "The image tool's account could not be matched to its Codex turn. Retry in a new turn."))?;
            let asset_account = routes
                .assets
                .account(body, config["image_routing"]["mode"] == "auto")?;
            let target = match &routes.turns[index].target {
                Some(target) => target.clone(),
                None => {
                    let target = match config["image_routing"]["mode"].as_str().unwrap_or("follow")
                    {
                        "account" => Target::Account(
                            config["image_routing"]["account_id"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                        ),
                        "auto" => Target::Account(match &asset_account {
                            Some(account) => account.clone(),
                            None => selection::automatic(state, &config, &routes.limited)?,
                        }),
                        _ => Target::Follow {
                            model: routes.turns[index].model.clone(),
                            provider: routes.turns[index].provider.clone(),
                        },
                    };
                    routes.turns[index].target = Some(target.clone());
                    target
                }
            };
            (target, asset_account)
        };
        let route = match target {
            Target::Account(account) => selection::account_route(state, &config, &account, model),
            Target::Follow {
                model: selected,
                provider,
            } => {
                let prepared =
                    prepare_request(state, json!({"model":selected}), RequestOperation::Images)
                        .map_err(|_| {
                            NativeHttpError::router(
                                409,
                                "The image tool's selected account is no longer available",
                            )
                        })?;
                if prepared.route.provider_id != provider
                    || prepared.route.dialect != Dialect::CodexNative
                {
                    return Err(NativeHttpError::router(
                        422,
                        "The selected service does not support Codex image tools",
                    ));
                }
                image_route(
                    prepared.route.provider.value(),
                    prepared.route.source,
                    model,
                )
            }
        }?;
        if asset_account
            .as_deref()
            .is_some_and(|account| affinity::account_id(&route) != Some(account))
        {
            return Err(affinity::reference_error());
        }
        Ok(route)
    }

    pub(super) fn observe(
        &self,
        route: &ResolvedRoute,
        status: u16,
        headers: &BTreeMap<String, String>,
        body: Option<&Value>,
    ) {
        if (200..300).contains(&status)
            && let Some(body) = body
        {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .assets
                .remember(route, body);
        }
        let exhausted = ["primary", "secondary"].into_iter().any(|window| {
            headers
                .get(&format!("x-image-gen-{window}-used-percent"))
                .and_then(|value| value.parse::<f64>().ok())
                .is_some_and(|used| used >= 100.0)
        });
        if status != 429 && !exhausted {
            return;
        }
        let id = if route.provider.value().get("implicit_native") == Some(&Value::Bool(true)) {
            "@native"
        } else {
            &route.provider_id
        };
        let reset = body.and_then(|value| {
            value["error"]["resets_at"]
                .as_f64()
                .map(|reset| reset - crate::util::system_now())
        });
        let header_reset = ["primary", "secondary"]
            .into_iter()
            .filter_map(|window| headers.get(&format!("x-image-gen-{window}-reset-at")))
            .filter_map(|value| value.parse::<f64>().ok())
            .map(|reset| reset - crate::util::system_now())
            .filter(|delay| delay.is_finite())
            .reduce(f64::max);
        let delay = headers
            .get("retry-after")
            .and_then(|value| value.parse::<f64>().ok())
            .into_iter()
            .chain(reset)
            .chain(header_reset)
            .filter(|delay| delay.is_finite() && *delay > 0.0)
            .reduce(f64::max)
            .filter(|delay| delay.is_finite())
            .unwrap_or(300.0)
            .clamp(1.0, 604800.0);
        let mut routes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        routes.limited.retain(|_, until| *until > Instant::now());
        if routes.limited.len() < CAPACITY || routes.limited.contains_key(id) {
            routes.limited.insert(
                id.to_owned(),
                Instant::now() + Duration::from_secs_f64(delay),
            );
        }
    }
}

pub(super) fn image_route(
    provider: &Map<String, Value>,
    source: RouteSource,
    model: &str,
) -> Result<ResolvedRoute, NativeHttpError> {
    let prefix = provider
        .get("account")
        .and_then(|account| account["prefix"].as_str());
    let requested = prefix.map_or_else(|| model.to_owned(), |prefix| format!("{prefix}/{model}"));
    resolved_route_from_parts(
        &requested,
        provider.clone(),
        json!({"id":requested,"upstream_id":model})
            .as_object()
            .unwrap()
            .clone(),
        source,
    )
    .map_err(|_| NativeHttpError::router(500, "image route could not be prepared"))
}
