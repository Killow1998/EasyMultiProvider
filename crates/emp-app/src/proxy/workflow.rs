use crate::app::ServerState;
use crate::modules::hooks::HookOperation;
use crate::server::disconnect::{DisconnectMonitor, WebSocketDisconnect};
use crate::services::history::{
    DestinationPrepareError, prepare_destination_context, prepare_history,
};
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError, prepare_request,
};
use emp_context::HistoryError;
use emp_domain::ResolvedRoute;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::task::block_in_place;

pub(crate) enum PrepareFailure<'a> {
    Request(RequestPreparationError),
    History(HistoryError),
    Destination(DestinationPrepareError, &'a ResolvedRoute),
}

pub(crate) fn resolve(
    state: &ServerState,
    body: Value,
    operation: RequestOperation,
    incoming: &BTreeMap<String, String>,
    hooks: Option<&HookOperation>,
    observation: &mut RequestObservation,
) -> Result<PreparedRequest, PrepareFailure<'static>> {
    let prepared = match operation {
        RequestOperation::Responses => {
            crate::services::memory_routing::prepare(state, body, incoming, hooks)
        }
        RequestOperation::Compact | RequestOperation::Images => {
            prepare_request(state, body, operation, incoming, hooks)
        }
    }
    .map_err(PrepareFailure::Request)?;
    observation.selected(&prepared.route);
    Ok(prepared)
}

pub(crate) struct ContextPreparation<'a> {
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    headers: &'a BTreeMap<String, String>,
}

impl<'a> ContextPreparation<'a> {
    pub(crate) fn begin(
        state: &'a ServerState,
        route: &'a ResolvedRoute,
        headers: &'a BTreeMap<String, String>,
        observation: &mut RequestObservation,
    ) -> Self {
        observation.phase(Phase::PrepareHistory);
        Self {
            state,
            route,
            headers,
        }
    }

    pub(crate) fn prepare(
        self,
        body: Value,
        observation: &mut RequestObservation,
        disconnect: impl FnOnce() -> Option<DisconnectMonitor>,
    ) -> Result<Value, PrepareFailure<'a>> {
        let body = self.history(body)?;
        observation.phase(Phase::PrepareDestination);
        self.destination(body, disconnect().as_mut())
    }

    pub(crate) async fn prepare_websocket(
        self,
        body: Value,
        observation: &mut RequestObservation,
        disconnect: Option<&WebSocketDisconnect>,
    ) -> Result<Value, PrepareFailure<'a>> {
        let body = websocket_object(block_in_place(|| self.history(body))?)?;
        observation.phase(Phase::PrepareDestination);
        let body = if let Some(disconnect) = disconnect {
            disconnect
                .watch(|monitor| self.destination(body, Some(monitor)))
                .await
        } else {
            block_in_place(|| self.destination(body, None))
        }?;
        websocket_object(body)
    }

    fn history(&self, body: Value) -> Result<Value, PrepareFailure<'a>> {
        prepare_history(self.state, self.route, body, self.headers).map_err(PrepareFailure::History)
    }

    fn destination(
        &self,
        body: Value,
        monitor: Option<&mut DisconnectMonitor>,
    ) -> Result<Value, PrepareFailure<'a>> {
        prepare_destination_context(self.state, self.route, body, self.headers, monitor)
            .map_err(|error| PrepareFailure::Destination(error, self.route))
    }
}

fn websocket_object(body: Value) -> Result<Value, PrepareFailure<'static>> {
    if body.is_object() {
        Ok(body)
    } else {
        Err(PrepareFailure::History(HistoryError::new(
            "invalid_history_projection",
        )))
    }
}
