use crate::app::ServerState;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace, race};
use crate::services::providers::save_learned_protocol;
use crate::services::request_outcome::RequestGuard;
use emp_domain::{ResolvedRoute, RouteResolutionError};
use emp_router::failure_policy;
use emp_router::{CompleteResponse, ExternalRouter, ExternalStream, ProjectionIds, RouterError};
use emp_state::{config::LearnedProtocol, observed_at_now};
use emp_transport::{FailureClass, FailurePhase, UpstreamFailure};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub(crate) enum ExternalRequestError {
    Router(RouterError, Box<ResolvedRoute>),
    Route(RouteResolutionError),
    Unsupported,
    Disconnected,
}

pub(crate) fn complete(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<CompleteResponse, ExternalRequestError> {
    let started = Instant::now();
    let router = ExternalRouter::new(
        &state.backend.transport.client,
        state.backend.configuration.official_models,
    )
    .with_gemini_signatures(state.backend.transport.gemini_signatures.clone());
    let router = &router;
    let mut outcome =
        RequestGuard::new(state, route, body, incoming, None, "responses").started_at(started);
    let execution = Execution { state, route, body };
    let (result, candidate) = state.backend.transport.runtime.block_on(execution.run(
        None,
        &mut outcome,
        |candidate| async move {
            router
                .execute_complete(&candidate, body, incoming, ids)
                .await
        },
    ))?;
    let learned = LearnedProtocol::from_route(route, candidate.protocol, observed_at_now());
    outcome.http_status(result.status);
    outcome.upstream_observation(&result.observation);
    outcome.observe(&result.body);
    if result.body["status"] == "completed" {
        crate::services::context::record(state, &candidate, body, true);
    }
    save_learned_protocol(state, learned);
    Ok(result)
}

pub(crate) async fn open_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: Option<&mut DisconnectMonitor>,
    outcome: &mut RequestGuard<'_>,
) -> Result<(ExternalStream, ResolvedRoute), ExternalRequestError> {
    let router = ExternalRouter::new(
        &state.backend.transport.client,
        state.backend.configuration.official_models,
    )
    .with_gemini_signatures(state.backend.transport.gemini_signatures.clone());
    let router = &router;
    Execution { state, route, body }
        .run(monitor, outcome, |candidate| async move {
            router.open_stream(&candidate, body, incoming, ids).await
        })
        .await
}

struct Execution<'a> {
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    body: &'a Value,
}

impl Execution<'_> {
    async fn run<T, F: Future<Output = Result<T, RouterError>>>(
        &self,
        mut monitor: Option<&mut DisconnectMonitor>,
        outcome: &mut RequestGuard<'_>,
        mut request: impl FnMut(ResolvedRoute) -> F,
    ) -> Result<(T, ResolvedRoute), ExternalRequestError> {
        let Self { state, route, body } = *self;
        let candidates = emp_router::protocol_candidates(route);
        'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
            let candidate = route
                .with_protocol(protocol)
                .map_err(ExternalRequestError::Route)?;
            outcome.candidate(&candidate);
            for attempt in 0..3 {
                outcome.dispatch();
                let result = match race(monitor.as_deref_mut(), request(candidate.clone())).await {
                    DisconnectRace::Ready(result) => result,
                    DisconnectRace::Disconnected => {
                        outcome.disconnected_before_delivery();
                        return Err(ExternalRequestError::Disconnected);
                    }
                };
                let error = match result {
                    Ok(result) => return Ok((result, candidate)),
                    Err(error) => error,
                };
                if error.error_class() == FailureClass::ContextLengthExceeded {
                    tokio::task::block_in_place(|| {
                        crate::services::context::record(state, &candidate, body, false)
                    });
                }
                let retry = retry_delay(&error, attempt, &candidate);
                let fallback = retry.is_none()
                    && index + 1 < candidates.len()
                    && error.failure_reason() != Some("model_not_found")
                    && failure_policy::protocol_fallback_allowed(error.status(), false, false);
                if let Some(delay) = retry.or(fallback.then_some(Duration::ZERO)) {
                    outcome.retry(attempt, delay, &error, fallback);
                    if fallback {
                        continue 'candidate;
                    }
                    if matches!(
                        race(monitor.as_deref_mut(), tokio::time::sleep(delay)).await,
                        DisconnectRace::Disconnected
                    ) {
                        outcome.disconnected_before_delivery();
                        return Err(ExternalRequestError::Disconnected);
                    }
                    continue;
                }
                outcome.router_error(&error);
                return Err(ExternalRequestError::Router(error, Box::new(candidate)));
            }
        }
        outcome.status(503, "router_error");
        Err(ExternalRequestError::Unsupported)
    }
}

fn retry_delay(error: &RouterError, attempt: usize, route: &ResolvedRoute) -> Option<Duration> {
    let failure = UpstreamFailure {
        error_class: error.error_class(),
        status: error.status(),
        phase: FailurePhase::TerminalValidation,
        terminal_event: false,
        failure_reason: error.failure_reason().map(str::to_owned),
        retry_after_seconds: error.retry_after_seconds(),
    };
    let free_route = route
        .upstream_model
        .trim()
        .to_ascii_lowercase()
        .ends_with(":free");
    failure_policy::external_http_retry_allowed(&failure, attempt, false, false, free_route).then(
        || {
            failure
                .retry_after_seconds
                .map(Duration::from_secs)
                .unwrap_or_else(|| failure_policy::external_backoff_delay(attempt))
        },
    )
}
