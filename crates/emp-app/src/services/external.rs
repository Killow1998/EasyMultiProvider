//! External Responses execution. Callers choose complete or streaming delivery;
//! this module owns protocol selection, bounded retries and attempt accounting.
use crate::app::ServerState;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace, raced};
use crate::services::observation;
use crate::services::providers::persist_protocol_observation;
use crate::services::request_outcome::RequestOutcome;
use emp_core::{ResolvedRoute, RouteResolutionError};
use emp_router::{CompleteResponse, ExternalRouter, ExternalStream, ProjectionIds, RouterError};
use emp_transport::{FailureClass, FailurePhase, UpstreamFailure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ops::AsyncFnMut;
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
    let router = ExternalRouter::new(&state.backend.transport.client);
    let mut outcome =
        RequestOutcome::new(state, route, body, incoming, None, "responses").started_at(started);
    let mut execution = Execution {
        state,
        route,
        body,
        incoming,
        activity: None,
    };
    let (result, candidate) = execution.run(
        None,
        Some(&mut outcome),
        async |candidate: &ResolvedRoute| {
            router
                .execute_complete(candidate, body, incoming, ids)
                .await
        },
    )?;
    outcome.http_status(result.status);
    outcome.reported_model(result.reported_model.as_deref());
    outcome.observe(&result.body);
    if result.body["status"] == "completed" {
        crate::services::context::record(state, &candidate, body, true);
    }
    persist_protocol_observation(state, &candidate);
    Ok(result)
}

/// An opened stream transfers completion accounting to its relay. Opening
/// retries stop at this interface: no stream output is ever replayed here.
pub(crate) fn open_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: Option<&mut DisconnectMonitor>,
) -> Result<(ExternalStream, ResolvedRoute), ExternalRequestError> {
    let router = ExternalRouter::new(&state.backend.transport.client);
    Execution {
        state,
        route,
        body,
        incoming,
        activity: None,
    }
    .run(monitor, None, async |candidate: &ResolvedRoute| {
        router.open_stream(candidate, body, incoming, ids).await
    })
}

struct Execution<'a> {
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    body: &'a Value,
    incoming: &'a BTreeMap<String, String>,
    activity: Option<crate::services::activity::ActivityGuard<'a>>,
}

impl Execution<'_> {
    // A complete request owns one outcome across all attempts. A stream only
    // owns opening here; its relay owns the outcome after a successful open.
    fn run<T>(
        &mut self,
        mut monitor: Option<&mut DisconnectMonitor>,
        mut outcome: Option<&mut RequestOutcome<'_>>,
        mut request: impl AsyncFnMut(&ResolvedRoute) -> Result<T, RouterError>,
    ) -> Result<(T, ResolvedRoute), ExternalRequestError> {
        let Self {
            state,
            route,
            body,
            incoming,
            ..
        } = *self;
        let candidates = emp_router::protocol_candidates(route);
        let request_id = json!(
            incoming
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-emp-request-id"))
                .map(|(_, value)| value)
        );
        'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
            let candidate = route
                .with_protocol(protocol)
                .map_err(ExternalRequestError::Route)?;
            if let Some(outcome) = outcome.as_deref_mut() {
                outcome.candidate(&candidate);
            }
            for attempt in 0..3 {
                if let Some(outcome) = outcome.as_deref_mut() {
                    outcome.dispatch();
                    self.activity.get_or_insert_with(|| {
                        state.backend.activity.begin(
                            crate::services::activity::ActivityIdentity::from_route(route),
                        )
                    });
                } else {
                    observation::execution_attempt(state, &candidate, body, incoming);
                }
                let result = match raced(
                    &state.backend.transport.runtime,
                    monitor.as_deref_mut(),
                    request(&candidate),
                ) {
                    DisconnectRace::Ready(result) => result,
                    DisconnectRace::Disconnected => {
                        observation::request_cancelled(state, &candidate, incoming);
                        return Err(ExternalRequestError::Disconnected);
                    }
                };
                let error = match result {
                    Ok(result) => return Ok((result, candidate)),
                    Err(error) => error,
                };
                if error.error_class() == FailureClass::ContextLengthExceeded {
                    crate::services::context::record(state, &candidate, body, false);
                }
                let retry = retry_delay(&error, attempt, &candidate);
                let fallback = retry.is_none()
                    && index + 1 < candidates.len()
                    && emp_transport::protocol_fallback_allowed(error.status(), false, false);
                if let Some(delay) = retry.or(fallback.then_some(Duration::ZERO)) {
                    if let Some(outcome) = outcome.as_deref_mut() {
                        outcome.retry(attempt, delay, &error, fallback);
                    } else {
                        observation::retry_scheduled(
                            state,
                            &request_id,
                            attempt,
                            delay,
                            &error,
                            fallback,
                        );
                    }
                    if fallback {
                        continue 'candidate;
                    }
                    if matches!(
                        raced(
                            &state.backend.transport.runtime,
                            monitor.as_deref_mut(),
                            async { tokio::time::sleep(delay).await },
                        ),
                        DisconnectRace::Disconnected
                    ) {
                        observation::request_cancelled(state, &candidate, incoming);
                        return Err(ExternalRequestError::Disconnected);
                    }
                    continue;
                }
                if let Some(outcome) = outcome.as_deref_mut() {
                    outcome.router_error(&error);
                } else {
                    RequestOutcome::new(state, &candidate, body, incoming, None, "responses")
                        .router_error(&error);
                }
                return Err(ExternalRequestError::Router(error, Box::new(candidate)));
            }
        }
        if let Some(outcome) = outcome {
            outcome.status(503, "router_error");
        }
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
    emp_transport::external_http_retry_allowed(&failure, attempt, false, false, free_route).then(
        || {
            failure
                .retry_after_seconds
                .map(Duration::from_secs)
                .unwrap_or_else(|| emp_transport::external_backoff_delay(attempt))
        },
    )
}
