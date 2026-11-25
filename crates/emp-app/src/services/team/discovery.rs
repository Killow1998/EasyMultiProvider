use super::{Failure, Result, policy};
use crate::app::ServerState;
use crate::server::team_discovery::{AnnouncementSocket, find_announcements, local_networks};
use emp_modules::team::{DiscoveredWorkspace, Request};
use serde_json::{Value, json};
use std::{sync::Mutex, time::Duration};
use tokio::task::AbortHandle;

#[derive(Default)]
pub(super) struct Discovery {
    announcement: Mutex<Option<AbortHandle>>,
    #[cfg(all(test, unix))]
    no_broadcast_interfaces: std::sync::atomic::AtomicBool,
}

#[cfg(all(test, unix))]
impl super::TeamState {
    pub(crate) fn isolate_discovery_for_test(&self) {
        self.discovery
            .no_broadcast_interfaces
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Discovery {
    pub(super) fn announce(&self, state: &ServerState, origin: &str, reply: &Value) -> Result<()> {
        let mut announcement = self
            .announcement
            .lock()
            .map_err(|_| Failure::unavailable())?;
        if announcement
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Ok(());
        }
        let socket = AnnouncementSocket::bind().map_err(network_error)?;
        let networks = local_networks().map_err(network_error)?;
        let response = json!({"base":origin,"workspace":reply["workspace"]}).to_string();
        let active =
            Duration::from_secs(reply["expires_in"].as_u64().ok_or_else(Failure::invalid)?);
        let task = socket
            .announce(&state.backend.transport.runtime, networks, response, active)
            .map_err(network_error)?;
        *announcement = Some(task);
        Ok(())
    }

    pub(super) fn find(&self, state: &ServerState) -> Result<Value> {
        let networks = local_networks().map_err(network_error)?;
        #[cfg(all(test, unix))]
        let networks = if self
            .no_broadcast_interfaces
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            Vec::new()
        } else {
            networks
        };
        let found = state
            .backend
            .transport
            .runtime
            .block_on(find_announcements(&networks))
            .map_err(network_error)?;
        let workspaces: Vec<DiscoveredWorkspace> =
            policy(state, Request::Discovery { candidates: found })?;
        Ok(json!({"workspaces":workspaces}))
    }

    pub(super) fn stop(&self) {
        if let Ok(mut announcement) = self.announcement.lock()
            && let Some(task) = announcement.take()
        {
            task.abort();
        }
    }
}

fn network_error(_: std::io::Error) -> Failure {
    Failure(503, "workspace_discovery_unavailable".into())
}
